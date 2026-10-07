/** The last part of a path, such as `server.c`. */
export function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** A path relative to where uscope runs, when it is inside it. */
export function shortPath(path: string, cwd: string | undefined): string {
  if (cwd && path.startsWith(`${cwd}/`)) {
    return `./${path.slice(cwd.length + 1)}`;
  }
  return path;
}
