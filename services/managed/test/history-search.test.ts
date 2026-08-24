import { describe, expect, it } from "vitest";

import {
  HISTORY_VECTOR_MATCH_THRESHOLD,
  groupHistoryCitations,
  historyFtsQuery,
  historySearchTerms,
  historyVectorRetrieval,
  isAcceptedHistoryLexicalMatch,
  isExactHistoryIdentifierQuery,
  parseHistoryFindSessionsInput,
  parseHistoryReadSessionInput,
} from "../src/history-search";

describe("history search query routing", () => {
  it("keeps exact identifier queries on authoritative lexical search", () => {
    expect(isExactHistoryIdentifierQuery("MEMORY_STRESS_A11_T1")).toBe(true);
    expect(isExactHistoryIdentifierQuery("release:artifact_42")).toBe(true);
    expect(isExactHistoryIdentifierQuery("memory-scope architecture")).toBe(false);
  });

  it("configures account-owned vector rejection before result limiting", () => {
    expect(historyVectorRetrieval("owner-a", 8)).toEqual({
      retrieval_type: "vector",
      match_threshold: HISTORY_VECTOR_MATCH_THRESHOLD,
      max_num_results: 24,
      filters: { scope_id: { $eq: "owner-a" } },
      return_on_failure: false,
    });
    expect(HISTORY_VECTOR_MATCH_THRESHOLD).toBe(0.5);
  });

  it("requires meaningful lexical coverage", () => {
    expect(historySearchTerms("What is the Atlas cargo insurance policy number?")).toEqual([
      "atlas", "cargo", "insurance", "policy", "number",
    ]);
    expect(historyFtsQuery("What is the Atlas cargo insurance policy number?")).toBe(
      '"atlas" OR "cargo" OR "insurance" OR "policy" OR "number"',
    );
    expect(isAcceptedHistoryLexicalMatch(
      "What is the Atlas cargo insurance policy number?",
      "The Atlas schedule changed.",
    )).toBe(false);
    expect(isAcceptedHistoryLexicalMatch(
      "What is the Atlas cargo insurance policy number?",
      "Atlas insurance policy 42 covers the shipment.",
    )).toBe(true);
  });

  it("validates the public findSessions and readSession contracts", () => {
    expect(parseHistoryFindSessionsInput({ query: "  copper lighthouse ", limit: 4 })).toEqual({
      query: "copper lighthouse",
      limit: 4,
    });
    expect(parseHistoryReadSessionInput({
      session_id: "018f1f9a-7b3c-7a09-8000-000000000009",
      turn_ids: ["turn-1", "turn:2"],
    })).toEqual({
      session_id: "018f1f9a-7b3c-7a09-8000-000000000009",
      turn_ids: ["turn-1", "turn:2"],
    });
    expect(() => parseHistoryFindSessionsInput({
      query: "copper",
      limit: 4,
      agentic: false,
    })).toThrow("supported fields are query and limit");
    expect(() => parseHistoryReadSessionInput({ session_id: "not-a-session" }))
      .toThrow("invalid session id");
  });

  it("preserves citation coordinates while grouping session results", () => {
    expect(groupHistoryCitations([
      {
        session_id: "session-a",
        title: "Session A",
        turn_id: "turn-1",
        cursor: "7",
      },
      {
        session_id: "session-a",
        title: "Session A",
        turn_id: "turn-2",
        cursor: "9",
      },
    ])).toEqual([{
      thread_id: "session-a",
      title: "Session A",
      sources: [
        { turn_id: "turn-1", cursor: "7" },
        { turn_id: "turn-2", cursor: "9" },
      ],
    }]);
  });
});
