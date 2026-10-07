import { useState } from "react";
import { write } from "../storage";
import { useConnection, useModel } from "../store";

/** A steady color for a name, so each person keeps theirs. */
export function hue(name: string): number {
  let hash = 0;
  for (const char of name) {
    hash = (hash * 31 + (char.codePointAt(0) ?? 0)) >>> 0;
  }
  return hash % 360;
}

export function People() {
  const people = useModel((model) => model.people);
  const me = useModel((model) => model.hello?.connection);
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const mine = people.find((person) => person.connection === me);
  if (people.length === 0) {
    return null;
  }
  const others = people.length - 1;

  return (
    <div className="people">
      <ul className="avatars" data-testid="people" aria-label="People in this session">
        {people.map((person) => (
          <li
            key={person.connection}
            className={`avatar ${person.role}`}
            style={{ background: `hsl(${hue(person.name)} 55% 45%)` }}
            title={`${person.name}${person.connection === me ? " (you)" : ""} · can ${person.role}`}
          >
            {person.name.slice(0, 2)}
          </li>
        ))}
      </ul>
      {editing && mine ? (
        <input
          className="input"
          style={{ width: 140, padding: "2px 6px" }}
          aria-label="Your name"
          defaultValue={mine.name}
          // biome-ignore lint/a11y/noAutofocus: the field opens because the person asked to type
          autoFocus
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              setEditing(false);
            }
            if (event.key === "Enter") {
              const name = event.currentTarget.value.trim();
              setEditing(false);
              if (name) {
                write("uscope-name", name);
                void connection.request("setName", { name }).catch(() => undefined);
              }
            }
          }}
          onBlur={() => setEditing(false)}
        />
      ) : (
        <button
          type="button"
          className="tb"
          title="Change the name others see"
          onClick={() => setEditing(true)}
        >
          {mine?.name ?? "you"}
          {others > 0 && <span className="muted">+{others}</span>}
        </button>
      )}
    </div>
  );
}
