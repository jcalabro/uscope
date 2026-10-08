// Moving the tab's focus. Every move is a navigation, so the address bar is
// always a working link to what the tab shows.

import { useNavigate } from "@tanstack/react-router";
import { useCallback, useMemo } from "react";
import { useRequest } from "../data";
import { type At, followedLook, type Look, linkPath, recordedPath, taskSegment } from "../focus";
import { useModel } from "../store";
import { useFocus } from "./Workspace";

export type Go = (at: At | null, look?: Look, options?: { replace?: boolean }) => void;

/** Goes to a stop, thread or task, and frame, or to the session itself. */
export function useGo(): Go {
  const navigate = useNavigate();
  const { session, look: current } = useFocus();
  return useCallback(
    (at, look, options) => {
      // A new frame shows its own code unless the caller says what to show.
      const search = look ?? followedLook(current);
      const replace = options?.replace ?? false;
      if (at === null) {
        void navigate({ to: "/s/$session", params: { session }, search, replace });
        return;
      }
      if (at.task) {
        void navigate({
          to: "/s/$session/stop/$stop/task/$task/f/$frame",
          params: {
            session,
            stop: String(at.stop),
            task: taskSegment(at.task),
            frame: String(at.frame),
          },
          search,
          replace,
        });
        return;
      }
      void navigate({
        to: "/s/$session/stop/$stop/t/$thread/f/$frame",
        params: {
          session,
          stop: String(at.stop),
          thread: String(at.thread),
          frame: String(at.frame),
        },
        search,
        replace,
      });
    },
    [navigate, session, current],
  );
}

/** Changes how the tab looks at its focus, keeping the focus. */
export function useLook(): (change: (look: Look) => Look, options?: { replace?: boolean }) => void {
  const go = useGo();
  const { at, look } = useFocus();
  return useCallback(
    (change, options) => go(at, change(look), { replace: options?.replace ?? true }),
    [go, at, look],
  );
}

/**
 * Turns recorded source paths into the short ones links carry, and back,
 * against the files the debug information records.
 */
export function useLinkPaths() {
  const cwd = useModel((model) => model.hello?.cwd);
  const session = useModel((model) => model.state?.session);
  const files = useRequest("sources", session ? undefined : null).data?.files;
  return useMemo(
    () => ({
      link: (path: string) => linkPath(path, cwd, files),
      recorded: (path: string) => recordedPath(path, cwd, files),
    }),
    [cwd, files],
  );
}
