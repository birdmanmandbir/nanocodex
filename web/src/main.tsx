import { preloadDirectSurface } from "./routeLoaders";

const directUrl = new URL(window.location.href);
const directPath = directUrl.pathname === "/"
  ? "/"
  : directUrl.pathname.replace(/\/+$/, "");

if (directPath === "/artifact-runtime") {
  void import("./artifactRuntime");
} else if (directPath === "/ci") {
  void import("./Ci").then(({ CiStandalone }) => {
    createRoot(document.getElementById("root")!).render(<CiStandalone />);
  });
} else {
  const application = import("./NanocodexApp");
  void Promise.all([
    application,
    preloadDirectSurface(directUrl),
  ]).then(
    ([module, preparedRoute]) => module.mountNanocodexApp(preparedRoute),
    () => {
      // A failed route preparation must not strand the document. The normal
      // route lifecycle owns its actionable failure state and retry policy.
      void application.then((module) => module.mountNanocodexApp({}));
    },
  );
}
