/// <reference lib="DOM" />

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { AppsConsole } from "./AppsConsole";
import "./apps.css";

const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("Nanocodex Apps requires a #root element");

createRoot(rootElement).render(
  <StrictMode>
    <AppsConsole />
  </StrictMode>,
);
