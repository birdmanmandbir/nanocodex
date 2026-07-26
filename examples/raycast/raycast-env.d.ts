/// <reference types="@raycast/api">

/* 🚧 🚧 🚧
 * This file is auto-generated from the extension's manifest.
 * Do not modify manually. Instead, update the `package.json` file.
 * 🚧 🚧 🚧 */

/* eslint-disable @typescript-eslint/ban-types */

type ExtensionPreferences = {}

/** Preferences accessible in all the extension's commands */
declare type Preferences = ExtensionPreferences

declare namespace Preferences {
  /** Preferences accessible in the `ask-nanocodex` command */
  export type AskNanocodex = ExtensionPreferences & {}
  /** Preferences accessible in the `run-nanocodex-job` command */
  export type RunNanocodexJob = ExtensionPreferences & {}
}

declare namespace Arguments {
  /** Arguments passed to the `ask-nanocodex` command */
  export type AskNanocodex = {
  /** Ask anything */
  "prompt": string
}
  /** Arguments passed to the `run-nanocodex-job` command */
  export type RunNanocodexJob = {}
}

