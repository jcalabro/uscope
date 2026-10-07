// The page's theme: the system's, or light or dark as this browser chose.
// The stylesheet reads `data-theme` on the root element.

import { read, write } from "./storage";

export type Theme = "system" | "light" | "dark";

export const THEMES: readonly Theme[] = ["system", "light", "dark"];

const isTheme = (value: unknown): value is Theme => THEMES.includes(value as Theme);

export function savedTheme(): Theme {
  return read("uscope-theme", "system", isTheme);
}

/** Shows `theme`, and remembers it for the next visit. */
export function chooseTheme(theme: Theme): void {
  apply(theme);
  write("uscope-theme", theme);
}

export function apply(theme: Theme): void {
  if (theme === "system") {
    document.documentElement.removeAttribute("data-theme");
  } else {
    document.documentElement.setAttribute("data-theme", theme);
  }
}
