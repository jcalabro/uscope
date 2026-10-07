import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { type ActionName, action, continueLabel } from "../actions";
import { describe } from "../keys";
import { targetName } from "../model";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { chooseTheme, THEMES, type Theme, useChosenTheme } from "../theme";
import { People } from "./People";
import { Share } from "./Share";
import { Status } from "./Status";

const BUTTONS: { name: ActionName; glyph: string; label?: string }[] = [
  { name: "continue", glyph: "▶" },
  { name: "pause", glyph: "⏸", label: "Pause" },
  { name: "over", glyph: "↷", label: "Over" },
  { name: "into", glyph: "↓", label: "Into" },
  { name: "out", glyph: "↑", label: "Out" },
  { name: "restart", glyph: "⟲", label: "Restart" },
  { name: "kill", glyph: "■", label: "Kill" },
];

export function Toolbar() {
  const model = useModel((model) => model);
  const connection = useConnection();
  const navigate = useNavigate();
  const [failure, setFailure] = useState<string | null>(null);
  const control = model.hello?.role === "control";
  const at = useTab((current) => current.at);

  return (
    <header className="toolbar">
      <Link to="/" className="brand" aria-label="uscope home">
        <img src="/favicon.svg" alt="" />
        uscope
      </Link>
      {!control && model.hello && (
        <span className="pill idle" title="This link can watch the session but not run or stop it">
          View only
        </span>
      )}
      {control &&
        BUTTONS.map(({ name, glyph, label }) => {
          const current = action(name, model, at);
          const key = describe(name);
          const primary =
            (name === "continue" || name === "pause") &&
            current.enabled &&
            (name === "pause" || model.state?.inferior.state !== "running");
          return (
            <button
              key={name}
              type="button"
              className={`tb ${primary ? "primary" : ""}`}
              disabled={!current.enabled}
              title={`${label ?? continueLabel(model)}${key ? ` (${key})` : ""}${current.enabled ? "" : `: ${current.reason}`}`}
              data-action={name}
              onClick={() => {
                if (current.enabled) {
                  setFailure(null);
                  current.run(connection).catch((error: Error) => setFailure(error.message));
                }
              }}
            >
              <span className="glyph">{glyph}</span>
              <span className={name === "continue" || name === "pause" ? "" : "label"}>
                {label ?? continueLabel(model)}
              </span>
              {key && <span className="hint">{key}</span>}
            </button>
          );
        })}
      <span className="separator" />
      <Status />
      {failure && (
        <span className="pill warn" role="alert" title={failure}>
          {failure.length > 60 ? `${failure.slice(0, 60)}…` : failure}
        </span>
      )}
      <span className="spacer" />
      <People />
      <Share />
      {control && (
        <button
          type="button"
          className="tb ghost"
          onClick={() => navigate({ to: "/pick" })}
          title="Launch, attach, or open a core dump"
        >
          {targetName(model.state) ? "Debug something else" : "Debug something"}
        </button>
      )}
      <ThemeButton />
      <button
        type="button"
        className="tb ghost"
        aria-label="Every key"
        title="Every key (?)"
        onClick={() => tab.setState((current) => ({ help: !current.help }))}
      >
        ?
      </button>
    </header>
  );
}

/** A circle, half filled for the system's theme, empty for light, full for dark. */
function ThemeGlyph({ theme }: { theme: Theme }) {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
      <circle
        cx="7"
        cy="7"
        r="5.5"
        fill={theme === "dark" ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth="1.5"
      />
      {theme === "system" && <path d="M7 1.5a5.5 5.5 0 0 1 0 11z" fill="currentColor" />}
    </svg>
  );
}

/** Cycles the theme: the system's, light, then dark. */
function ThemeButton() {
  const theme = useChosenTheme();
  const next = THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length] as Theme;
  return (
    <button
      type="button"
      className="tb ghost"
      aria-label={`Theme: ${theme}`}
      title={`Theme: ${theme}; click for ${next}`}
      onClick={() => chooseTheme(next)}
    >
      <ThemeGlyph theme={theme} />
    </button>
  );
}
