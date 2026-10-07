// The URL is the focus: each route is a place in the session that a link
// can name.

import {
  createRootRoute,
  createRoute,
  createRouter,
  type RouterHistory,
} from "@tanstack/react-router";
import { Picker, pickerSearch } from "./ui/Picker";
import { Home, Join, SessionPage } from "./ui/pages";
import { Shell } from "./ui/Shell";

const rootRoute = createRootRoute({ component: Shell });

const homeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: Home });

const joinRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/join",
  validateSearch: (search: Record<string, unknown>): { to?: string } =>
    typeof search.to === "string" ? { to: search.to } : {},
  component: Join,
});

const pickRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/pick",
  validateSearch: pickerSearch,
  component: Picker,
});

const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/s/$session",
  component: SessionPage,
});

const routeTree = rootRoute.addChildren([homeRoute, joinRoute, pickRoute, sessionRoute]);

export function createAppRouter(history?: RouterHistory) {
  return createRouter({ routeTree, ...(history ? { history } : {}) });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
