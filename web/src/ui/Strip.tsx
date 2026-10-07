import { useModel } from "../store";

/** The bottom strip: what others did, most recent first. */
export function Strip() {
  const notices = useModel((model) => model.notices);
  const me = useModel((model) => model.hello?.connection);
  const control = useModel((model) => model.hello?.role === "control");
  const latest = [...notices].reverse().find((notice) => notice.connection !== me);
  return (
    <footer className="strip" aria-live="polite">
      {latest ? (
        <span className="notice" data-testid="notice">
          <b>{latest.name}</b> {latest.text}{" "}
          <span className="muted">{new Date(latest.at).toLocaleTimeString()}</span>
        </span>
      ) : (
        control && (
          <span className="muted">
            <kbd>F5</kbd> run or continue · <kbd>F6</kbd> pause · <kbd>⇧F5</kbd> kill
          </span>
        )
      )}
    </footer>
  );
}
