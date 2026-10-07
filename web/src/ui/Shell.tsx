import { Outlet, useLocation } from "@tanstack/react-router";
import { useEffect } from "react";
import { action } from "../actions";
import { commandFor, isTextBox } from "../keys";
import { targetName } from "../model";
import { useConnection, useModel, useStoreApi } from "../store";
import { NeedsLink } from "./pages";
import { Strip } from "./Strip";
import { Toolbar } from "./Toolbar";

export function Shell() {
  const link = useModel((model) => model.link);
  const name = useModel((model) => targetName(model.state));
  const inferior = useModel((model) => model.state?.inferior.state);
  const location = useLocation();
  useKeys();

  useEffect(() => {
    const state = inferior === "stopped" ? "stopped" : inferior === "running" ? "running" : null;
    document.title = name ? `${name}${state ? ` · ${state}` : ""} · uscope` : "uscope";
  }, [name, inferior]);

  // Pages outside a session have nothing to control.
  const bare =
    location.pathname === "/join" ? (
      <Outlet />
    ) : link === "unauthorized" ? (
      <NeedsLink reason="This browser has not joined this uscope session yet." />
    ) : link === "incompatible" ? (
      <NeedsLink reason="This page is from another version of uscope. Reload to get this one's." />
    ) : null;
  if (bare) {
    return (
      <div className="app" style={{ gridTemplateRows: "1fr" }}>
        {bare}
      </div>
    );
  }
  return (
    <div className="app">
      <Toolbar />
      <Outlet />
      <Strip />
    </div>
  );
}

/** Runs the debugger keys, claiming them from the browser while it does. */
function useKeys() {
  const store = useStoreApi();
  const connection = useConnection();
  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      const command = commandFor(event, isTextBox(document.activeElement));
      if (command === null || command === "palette" || command === "help") {
        return;
      }
      // Claimed even when unavailable, so F5 never reloads the debugger away.
      event.preventDefault();
      const current = action(command, store.getState());
      if (current.enabled) {
        void current.run(connection).catch(() => undefined);
      }
    };
    window.addEventListener("keydown", listener, { capture: true });
    return () => window.removeEventListener("keydown", listener, { capture: true });
  }, [store, connection]);
}
