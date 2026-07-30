export type BrowserTool = {
  description: string;
  parameters: Record<string, unknown>;
  handler: (
    input: unknown,
    context: { sessionId: string },
  ) => unknown | Promise<unknown>;
};

export type BrowserToolMap = Record<string, BrowserTool>;

export type BrowserWebSocketRequest = {
  accountId?: string | undefined;
  /** Resolved credential for this handshake. Do not retain or log it. */
  bearerToken: string;
  fedramp?: boolean | undefined;
  turnState?: string | undefined;
};

export type BrowserWebSocketConnection = {
  socket: WebSocket;
  status?: number | undefined;
  requestId?: string | undefined;
  serverModel?: string | undefined;
  reasoningIncluded?: boolean | undefined;
  turnState?: string | undefined;
};

export function createBrowserHost(options?: {
  WebSocketImpl?: typeof WebSocket;
  createWebSocket?: (
    endpoint: string,
    sessionId: string,
    request: BrowserWebSocketRequest,
  ) => WebSocket | BrowserWebSocketConnection | Promise<WebSocket | BrowserWebSocketConnection>;
  onEvent?: (eventJson: string) => void;
  tools?: BrowserToolMap;
  maxQueuedMessages?: number;
  maxQueuedBytes?: number;
  maxBufferedSendBytes?: number;
}): unknown;
