import type { Model } from "../model";
import { targetName } from "../model";
import { useModel } from "../store";

interface Shown {
  pill: "stopped" | "running" | "idle" | "warn";
  label: string;
  detail: string | null;
}

/** What the status shows: the run state is the loudest thing on screen. */
export function shown(model: Model): Shown {
  if (model.link === "reconnecting" || model.link === "connecting") {
    return {
      pill: "warn",
      label: model.link === "connecting" ? "Connecting" : "Reconnecting",
      detail: model.state?.session ? "showing the last state" : null,
    };
  }
  const state = model.state;
  if (state?.busy) {
    return { pill: "warn", label: "Working", detail: state.busy };
  }
  if (!state?.target) {
    return { pill: "idle", label: "Nothing loaded", detail: null };
  }
  const inferior = state.inferior;
  switch (inferior.state) {
    case "notStarted":
      return { pill: "idle", label: "Not started", detail: "F5 runs it" };
    case "running":
      return { pill: "running", label: "Running", detail: `process ${inferior.pid}` };
    case "stopped":
      return {
        pill: "stopped",
        label: state.target.kind === "core" ? "Core dump" : "Stopped",
        detail: inferior.reason.description,
      };
    case "exited":
      return { pill: "idle", label: "Exited", detail: inferior.description };
    case "detached":
      return { pill: "idle", label: "Detached", detail: `process ${inferior.pid} runs on` };
  }
}

export function Status() {
  const model = useModel((model) => model);
  const { pill, label, detail } = shown(model);
  const program = targetName(model.state);
  return (
    <div className="status" data-testid="status">
      {program && (
        <span className="program" title={model.state?.target?.program}>
          {program}
          {model.state?.target?.arguments.length
            ? ` ${model.state.target.arguments.join(" ")}`
            : ""}
        </span>
      )}
      <span className={`pill ${pill}`} data-state={pill}>
        {label}
      </span>
      {detail && <span title={detail}>{detail}</span>}
    </div>
  );
}
