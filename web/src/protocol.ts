// Typed access to the generated protocol: which methods exist, what each
// takes, and what each answers.

import type * as P from "./protocol.gen";

export type * from "./protocol.gen";
export { PROTOCOL_VERSION } from "./protocol.gen";

export type Method = P.Request["method"];

/** The parameters a method takes, or `undefined` for none. */
export type ParamsOf<M extends Method> =
  Extract<P.Request, { method: M }> extends { params: infer T } ? T : undefined;

/** What each method answers. */
export interface Results {
  setName: null;
  share: P.ShareLink;
  completePath: P.PathCompletions;
  processes: P.Processes;
  launch: null;
  attach: null;
  openCore: null;
  end: null;
  continue: null;
  pause: null;
  kill: null;
  restart: null;
  step: null;
  setFocus: null;
  backtrace: P.Backtrace;
  sources: P.SourceFiles;
  source: P.SourceText;
  addBreakpoint: P.BreakpointAdded;
  editBreakpoint: null;
  removeBreakpoint: null;
  input: null;
}

// Fails to compile when a method has no entry in Results, or Results names
// one the server does not have.
type Exactly<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
const resultsCoverEveryMethod: Exactly<keyof Results, Method> = true;
void resultsCoverEveryMethod;

/** Kinds of failure the page handles, plus the connection's own. */
export type FailureKind = P.ErrorKind | "disconnected";

/** A request that failed, with the server's explanation. */
export class RequestError extends Error {
  readonly kind: FailureKind;

  constructor(kind: FailureKind, message: string) {
    super(message);
    this.kind = kind;
    this.name = "RequestError";
  }
}
