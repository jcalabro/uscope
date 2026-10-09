// The path the page is served under: `/`, or a reverse proxy's path such
// as `/debug/7/`, which the server names in a meta tag (`--public-url`).

function read(): string {
  const content = document.querySelector<HTMLMetaElement>('meta[name="uscope-base"]')?.content;
  if (!content?.startsWith("/")) {
    return "/";
  }
  return content.endsWith("/") ? content : `${content}/`;
}

export const base = typeof document === "undefined" ? "/" : read();

/** The router's path for a path of this page: the same, less `base`. */
export function withinBase(path: string): string {
  return base !== "/" && path.startsWith(base) ? `/${path.slice(base.length)}` : path;
}
