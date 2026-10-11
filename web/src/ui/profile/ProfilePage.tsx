import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useCallback, useMemo, useState } from "react";
import { base } from "../../base";
import {
  type ProfileLook,
  type ProfileView,
  parseRange,
  parseTransform,
  queryOf,
  untransformed,
  withLook,
} from "../../profile/look";
import { formatDuration, formatPercent, formatWeight } from "../../profile/paint";
import type { ProfileGranularity, ProfileInfo, ProfileSummary } from "../../protocol";
import { useConnection, useModel } from "../../store";
import { ProfileContext, type ProfileFocus, useAnswer, useProfile } from "./context";
import { FlameView } from "./FlameView";
import { FunctionsView } from "./FunctionsView";
import { Timeline } from "./Timeline";
import { TreeView } from "./TreeView";
import { ZonesView } from "./ZonesView";

/** `/profile/$profile`: one profile, explored. */
export function ProfilePage() {
  const { profile } = useParams({ from: "/profile/$profile" });
  const look = useSearch({ from: "/profile/$profile" });
  const navigate = useNavigate();
  const info = useModel((model) => model.profiles.find((each) => each.id === profile));
  // Each revision of a growing profile is asked for anew; the last one's
  // answers stay on screen until the new ones come.
  const revision = info?.revision ?? 0;
  const summary = useAnswer("profile", { profile, revision });
  const setLook = useCallback(
    (change: (look: ProfileLook) => ProfileLook, push = false) =>
      void navigate({
        to: "/profile/$profile",
        params: { profile },
        search: change(look),
        replace: !push,
      }),
    [navigate, profile, look],
  );
  const focus = useMemo<ProfileFocus | null>(
    () =>
      summary.data
        ? {
            profile,
            summary: summary.data,
            look,
            query: { ...queryOf(profile, look), revision },
            unit: look.src === "waits" ? "nanoseconds" : summary.data.unit,
            setLook,
          }
        : null,
    [profile, summary.data, look, setLook, revision],
  );

  if (summary.error) {
    return (
      <div className="page">
        <div className="card" style={{ width: "min(560px, 100%)" }} data-testid="profile-missing">
          <div className="card-head">
            <h1>No such profile</h1>
          </div>
          <div className="card-body">
            <p style={{ margin: 0 }}>{summary.error.message}.</p>
            <p className="muted" style={{ margin: 0 }}>
              Serve it with <code>uscope web PROFILE.usprof</code>.
            </p>
          </div>
        </div>
      </div>
    );
  }
  if (!focus) {
    return <div className="page muted">Reading the profile…</div>;
  }
  const view = look.view ?? "flame";
  return (
    <ProfileContext.Provider value={focus}>
      <div className="profile" data-testid="profile">
        {info ? <Recording info={info} /> : null}
        <ProfileHead />
        {focus.summary.timed ? (
          <Timeline />
        ) : (
          <div className="profile-untimed muted" data-testid="untimed">
            Read from {focus.summary.source ?? "a file"}, which keeps no times or threads: this
            profile has no timeline.
          </div>
        )}
        <div className="profile-view">
          {view === "zones" ? (
            <ZonesView />
          ) : view === "functions" ? (
            <FunctionsView />
          ) : view === "tree" ? (
            <TreeView />
          ) : (
            <FlameView />
          )}
        </div>
        <ProfileFoot summary={focus.summary} />
      </div>
    </ProfileContext.Provider>
  );
}

const VIEWS: { view: ProfileView; label: string }[] = [
  { view: "flame", label: "Flame graph" },
  { view: "tree", label: "Call tree" },
  { view: "functions", label: "Functions" },
  { view: "zones", label: "Zones" },
];

const GRANULARITIES: { by: ProfileGranularity; label: string }[] = [
  { by: "function", label: "functions" },
  { by: "physical", label: "functions, inlines folded" },
  { by: "line", label: "lines" },
  { by: "address", label: "instructions" },
];

/** The profile's name, the views, and everything the query holds, each
 * removable where it shows. */
function ProfileHead() {
  const { summary, look, setLook } = useProfile();
  const range = look.range ? parseRange(look.range) : null;
  const snapshots = summary.snapshots > 0;
  const taskName = (id: string) =>
    id === "none"
      ? "no task known"
      : (summary.tasks.find((task) => String(task.row) === id)?.name ?? `task ${id}`);
  const program = summary.program?.slice(summary.program.lastIndexOf("/") + 1);
  return (
    <div className="profile-head">
      <div className="profile-title">
        <span className="program" title={summary.program ?? undefined}>
          {program ?? summary.name}
        </span>
        <span className="muted">
          {summary.name} · {summary.timed ? `${formatDuration(summary.duration)} · ` : ""}
          {summary.samples.toLocaleString("en-US")} samples
        </span>
      </div>
      <div className="segmented">
        {VIEWS.filter(
          ({ view }) =>
            view !== "zones" || summary.zoneSites.length > 0 || summary.probes.plotted > 0,
        ).map(({ view, label }) => (
          <button
            key={view}
            type="button"
            aria-pressed={(look.view ?? "flame") === view}
            onClick={() =>
              setLook((look) => withLook(look, "view", view === "flame" ? undefined : view), true)
            }
          >
            {label}
          </button>
        ))}
      </div>
      {snapshots ? (
        <div className="segmented">
          {(
            [
              [undefined, "CPU", "Where the program ran"],
              ["waits", "Waits", "Where task snapshots found tasks waiting"],
            ] as const
          ).map(([src, label, title]) => (
            <button
              key={label}
              type="button"
              title={title}
              aria-pressed={look.src === src}
              onClick={() => setLook((look) => withLook(look, "src", src), true)}
            >
              {label}
            </button>
          ))}
        </div>
      ) : null}
      <div className="profile-query">
        {range ? (
          <Chip
            label={`${formatDuration(range[0])} – ${formatDuration(range[1])}`}
            title="Samples in this time range"
            remove={() => setLook((look) => withLook(look, "range", undefined))}
          />
        ) : null}
        {look.th ? (
          <Chip
            label={`${look.th.length} ${look.th.length === 1 ? "thread" : "threads"}`}
            title="Samples of these threads"
            remove={() => setLook((look) => withLook(look, "th", undefined))}
          />
        ) : null}
        {look.tk ? (
          <Chip
            label={look.tk.length === 1 ? taskName(look.tk[0] ?? "") : `${look.tk.length} tasks`}
            title={look.tk.map(taskName).join("\n")}
            remove={() => setLook((look) => withLook(look, "tk", undefined))}
          />
        ) : null}
        {(look.lb ?? []).map((label) => (
          <Chip
            key={label}
            label={label}
            title="Samples that carry this label"
            remove={() =>
              setLook((look) => {
                const kept = (look.lb ?? []).filter((each) => each !== label);
                return withLook(look, "lb", kept.length > 0 ? kept : undefined);
              })
            }
          />
        ))}
        {(look.tf ?? []).map((entry, index) => {
          const transform = parseTransform(entry);
          return transform ? (
            <Chip
              key={entry}
              label={`${transform.op} ${transform.name}`}
              title="Remove this transform and those after it"
              remove={() => setLook((look) => untransformed(look, index), true)}
            />
          ) : null;
        })}
        <label className="check">
          <input
            type="checkbox"
            checked={look.invert === "1"}
            onChange={(event) =>
              setLook((look) => withLook(look, "invert", event.target.checked ? "1" : undefined))
            }
          />
          Inverted
        </label>
        <label className="check" title="Show each async task's frames as a stack of its own">
          <input
            type="checkbox"
            checked={look.native !== "1"}
            onChange={(event) =>
              setLook((look) => withLook(look, "native", event.target.checked ? undefined : "1"))
            }
          />
          Async stacks
        </label>
        {snapshots ? (
          <>
            <label className="check" title="Root each path at the task it ran or waited in">
              <input
                type="checkbox"
                checked={look.bt === "1"}
                onChange={(event) =>
                  setLook((look) => withLook(look, "bt", event.target.checked ? "1" : undefined))
                }
              />
              By task
            </label>
            {look.src === "waits" ? null : (
              <label className="check" title="A timeline lane per task, not per thread">
                <input
                  type="checkbox"
                  checked={look.lanes === "tasks"}
                  onChange={(event) =>
                    setLook((look) =>
                      withLook(look, "lanes", event.target.checked ? "tasks" : undefined),
                    )
                  }
                />
                Task lanes
              </label>
            )}
          </>
        ) : null}
        {summary.labelKeys.length > 0 ? (
          <select
            className="input profile-by"
            aria-label="Root by label"
            value={look.bl ?? ""}
            onChange={(event) =>
              setLook((look) => withLook(look, "bl", event.target.value || undefined))
            }
          >
            <option value="">no label roots</option>
            {summary.labelKeys.map((key) => (
              <option key={key} value={key}>
                rooted by {key}
              </option>
            ))}
          </select>
        ) : null}
        <select
          className="input profile-by"
          aria-label="Frames by"
          value={look.by ?? "function"}
          onChange={(event) => {
            const by = event.target.value as ProfileGranularity;
            setLook((look) => withLook(look, "by", by === "function" ? undefined : by));
          }}
        >
          {GRANULARITIES.map(({ by, label }) => (
            <option key={by} value={by}>
              by {label}
            </option>
          ))}
        </select>
      </div>
    </div>
  );
}

function Chip({ label, title, remove }: { label: string; title: string; remove: () => void }) {
  return (
    <span className="chip profile-chip" title={title}>
      {label}
      <button type="button" aria-label={`Remove ${label}`} onClick={remove}>
        ×
      </button>
    </span>
  );
}

/** A recording's progress, and saving the profile as a file. */
function Recording({ info }: { info: ProfileInfo }) {
  const connection = useConnection();
  const control = useModel((model) => model.hello?.role === "control");
  const [failure, setFailure] = useState<string | null>(null);
  const save = async () => {
    setFailure(null);
    try {
      const response = await fetch(`${base}api/profile/${info.id}`, {
        method: "POST",
        credentials: "same-origin",
      });
      if (!response.ok) {
        throw new Error(`the server answered ${response.status}`);
      }
      const link = document.createElement("a");
      link.href = URL.createObjectURL(await response.blob());
      link.download = info.name.endsWith(".usprof") ? info.name : `${info.name}.usprof`;
      link.click();
      URL.revokeObjectURL(link.href);
    } catch (error) {
      setFailure(error instanceof Error ? error.message : String(error));
    }
  };
  return (
    <div className="profile-recording" data-testid="recording">
      {info.recording ? (
        <span className="pill running">Recording {info.name}</span>
      ) : info.ended ? (
        <span className="muted">
          {info.name} ended: {info.ended}
        </span>
      ) : null}
      {info.recording && control ? (
        <button
          type="button"
          className="tb ghost"
          onClick={() =>
            void connection
              .request("stopRecording", { profile: info.id })
              .catch((error: Error) => setFailure(error.message))
          }
        >
          Stop
        </button>
      ) : null}
      <span className="spacer" />
      {failure ? <span className="error">{failure}</span> : null}
      <button type="button" className="tb ghost" onClick={() => void save()}>
        Save
      </button>
    </div>
  );
}

/** What the profile cannot show, always in view. */
function ProfileFoot({ summary }: { summary: ProfileSummary }) {
  const truncated = summary.truncations.reduce((sum, each) => sum + each.count, 0);
  const setting = (name: string) => summary.settings.find((each) => each.name === name)?.value;
  const source = setting("source");
  const stack = setting("stack bytes");
  const paused = summary.gaps.reduce((sum, [from, to]) => sum + (to - from), 0);
  const span = Math.max(summary.duration, 1);
  const probesLost =
    summary.probes.overwritten +
    summary.probes.noRing +
    summary.probes.interrupted +
    summary.probes.malformed;
  return (
    <div className="profile-foot" data-testid="accounting">
      <span>{formatWeight(summary.weight, summary.unit)} sampled</span>
      {source ? <span>{source}</span> : null}
      {stack ? <span>{stack} B of stack copied</span> : null}
      <span className={summary.lost > 0 ? "warn" : undefined}>lost {summary.lost}</span>
      <span className={summary.throttles > 0 ? "warn" : undefined}>
        throttled {summary.throttles}
      </span>
      <span
        className={truncated > 0 ? "warn" : undefined}
        title={summary.truncations.map((each) => `${each.what}: ${each.count}`).join("\n")}
      >
        truncated {formatPercent(truncated, summary.samples)}
      </span>
      {summary.recorderCpu > 0 ? (
        <span
          title={`${formatDuration(summary.recorderCpu)} of CPU time recording, ${formatDuration(summary.recorderLoading)} of it reading debug information`}
        >
          uscope {formatPercent(summary.recorderCpu, span)} of a CPU
        </span>
      ) : null}
      {summary.gaps.length > 0 ? (
        <span
          className="warn"
          title={summary.budgetChanges
            .map((change) => `${formatDuration(change.time)}: ${change.text}`)
            .join("\n")}
        >
          paused {formatPercent(paused, span)} for the budget
        </span>
      ) : null}
      {summary.probes.events > 0 || probesLost > 0 ? (
        <span
          className={probesLost > 0 || summary.probeProblems.length > 0 ? "warn" : undefined}
          title={[
            `${summary.probes.overwritten} records overwritten before uscope read them`,
            `${summary.probes.noRing} events found no ring`,
            `${summary.probes.interrupted} events dropped by a signal handler`,
            `${summary.probes.malformed} records broke the probes ABI`,
            ...summary.probeProblems.map((problem) => problem.text),
          ].join("\n")}
        >
          {summary.probes.events.toLocaleString("en-US")} probe events, lost {probesLost}
        </span>
      ) : null}
      {summary.snapshots > 0 ? (
        <span
          className={summary.snapshotProblems.length > 0 ? "warn" : undefined}
          title={[
            `${formatPercent(summary.taskSamples, summary.samples)} of samples know their task`,
            ...summary.snapshotProblems.map(
              (problem) => `${formatDuration(problem.time)}: ${problem.text}`,
            ),
          ].join("\n")}
        >
          {summary.snapshots} task snapshots, stopped {formatPercent(summary.snapshotPause, span)}
        </span>
      ) : null}
    </div>
  );
}
