import { Link, Navigate, Outlet, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { login } from "../connection";
import { cache } from "../data";
import { isPagePath } from "../focus";
import { latestStop } from "../follow";
import { targetName } from "../model";
import { useConnection, useModel } from "../store";
import { recall } from "../tree";

/** `/`: wherever the session is. */
export function Home() {
  const state = useModel((model) => model.state);
  if (!state) {
    return <div className="page muted">Connecting to uscope…</div>;
  }
  if (state.session) {
    return <Navigate to="/s/$session" params={{ session: state.session }} replace />;
  }
  return <Navigate to="/pick" replace />;
}

// One trade per page load: a second render of the join page finds the
// token already gone from the address bar.
let joining: Promise<boolean> | null = null;

function joinOnce(): Promise<boolean> {
  if (!joining) {
    const token = window.location.hash.slice(1);
    // The token leaves the address bar before anything else can copy it.
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
    joining = login(token);
  }
  return joining;
}

/** `/join#TOKEN`: trades the token for a cookie, then opens the link. */
export function Join() {
  const connection = useConnection();
  const navigate = useNavigate();
  const { to } = useSearch({ from: "/join" });
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    joinOnce()
      .then((accepted) => {
        if (!accepted) {
          setFailed(true);
          return;
        }
        connection.start();
        // `href`, since the link holds a query of its own.
        void navigate({ href: isPagePath(to) ? to : "/", replace: true });
      })
      .catch(() => setFailed(true));
  }, [connection, navigate, to]);

  if (failed) {
    return <NeedsLink reason="This link is not for this uscope, or it has expired." />;
  }
  return <div className="page muted">Joining…</div>;
}

export function NeedsLink({ reason }: { reason: string }) {
  return (
    <div className="page">
      <div className="card" style={{ width: "min(560px, 100%)" }}>
        <div className="card-head">
          <h1>Open a join link</h1>
        </div>
        <div className="card-body">
          <p style={{ margin: 0 }}>{reason}</p>
          <p className="muted" style={{ margin: 0 }}>
            <code>uscope web</code> prints a link when it starts, and anyone in the session can make
            one with Share.
          </p>
        </div>
      </div>
    </div>
  );
}

/** `/s/$session`: the session, or why it is not here. */
export function SessionPage() {
  const { session } = useParams({ from: "/s/$session" });
  const state = useModel((model) => model.state);
  const notices = useModel((model) => model.notices);
  const navigate = useNavigate();
  // A tab that saw this session live follows it to the next one; a tab
  // opened on a link to an ended session says so instead.
  const followed = useRef(false);
  const current = state?.session ?? null;
  if (current === session) {
    followed.current = true;
  }

  useEffect(() => {
    if (followed.current && current && current !== session) {
      void navigate({ to: "/s/$session", params: { session: current }, replace: true });
    }
  }, [current, session, navigate]);

  // Answers belong to one session; the files a program has change as its
  // libraries load, so each stop asks again. Values change when someone
  // writes one, and their handles belong to one connection.
  const latest = state ? latestStop(state) : undefined;
  const writes = state?.writes;
  const connection = useModel((model) => model.hello?.connection);
  const cached = useRef({ current, latest, writes, connection });
  const before = cached.current;
  if (before.current !== current) {
    cache.clear();
    recall.clear();
  } else {
    if (before.latest !== latest) {
      cache.forget("sources ");
    }
    if (before.writes !== writes || before.connection !== connection) {
      for (const method of ["scopes ", "children ", "evaluate "]) {
        cache.forget(method);
      }
    }
  }
  cached.current = { current, latest, writes, connection };

  if (!state) {
    return <div className="page muted">Connecting to uscope…</div>;
  }
  if (current === session) {
    return <Outlet />;
  }
  if (state.busy) {
    return <div className="page muted">{state.busy}…</div>;
  }
  const ended = [...notices].reverse().find((notice) => notice.text.startsWith("ended"));
  return (
    <div className="page">
      <div className="card" style={{ width: "min(560px, 100%)" }} data-testid="session-ended">
        <div className="card-head">
          <h1>This session ended</h1>
        </div>
        <div className="card-body">
          <p style={{ margin: 0 }}>
            {ended ? `${ended.name} ${ended.text}. ` : ""}
            {current
              ? `uscope is now debugging ${targetName(state) ?? "something else"}.`
              : "uscope is not debugging anything now."}
          </p>
          <div className="actions">
            {current ? (
              <Link className="button primary" to="/s/$session" params={{ session: current }}>
                Go to {targetName(state)}
              </Link>
            ) : (
              <Link className="button primary" to="/pick">
                Debug something
              </Link>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
