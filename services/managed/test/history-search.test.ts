import { describe, expect, it } from "vitest";

import {
  HISTORY_VECTOR_MATCH_THRESHOLD,
  agenticHistoryEvidence,
  historyFtsQuery,
  historySearchTerms,
  historyVectorRetrieval,
  isAcceptedHistoryLexicalMatch,
  isExactHistoryIdentifierQuery,
  parseHistoryFindThreadsInput,
  parseHistoryReadThreadInput,
  seededAgenticSearchPrompt,
} from "../src/history-search";

describe("history search query routing", () => {
  it("keeps exact identifier queries on authoritative lexical search", () => {
    expect(isExactHistoryIdentifierQuery("MEMORY_STRESS_A11_T1")).toBe(true);
    expect(isExactHistoryIdentifierQuery("release:artifact_42")).toBe(true);
    expect(isExactHistoryIdentifierQuery("  MEMORY_STRESS_A11_T1  ")).toBe(true);
  });

  it("keeps prose and ordinary hyphenated terms eligible for vector search", () => {
    expect(isExactHistoryIdentifierQuery("gemstone waterfowl designation")).toBe(false);
    expect(isExactHistoryIdentifierQuery("memory-scope architecture")).toBe(false);
    expect(isExactHistoryIdentifierQuery("A11 T1")).toBe(false);
  });

  it("configures provider-side vector rejection before result limiting", () => {
    expect(historyVectorRetrieval("owner-a", 8)).toEqual({
      retrieval_type: "vector",
      match_threshold: HISTORY_VECTOR_MATCH_THRESHOLD,
      max_num_results: 24,
      filters: { scope_id: { $eq: "owner-a" } },
      return_on_failure: false,
    });
    expect(HISTORY_VECTOR_MATCH_THRESHOLD).toBe(0.5);
  });

  it("requires lexical coverage instead of accepting one crowded-in term", () => {
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
    expect(isAcceptedHistoryLexicalMatch("copper lighthouse", "copper only")).toBe(false);
    expect(isAcceptedHistoryLexicalMatch("copper lighthouse", "the copper lighthouse")).toBe(true);
    expect(isAcceptedHistoryLexicalMatch(
      "silver otter",
      "Otter habitats use silver ankle tags.",
    )).toBe(false);
    expect(isAcceptedHistoryLexicalMatch("what was it", "what was it")).toBe(false);
  });

  it("keeps exact identifiers eligible as a single lexical term", () => {
    expect(historyFtsQuery("COPPER_LIGHTHOUSE_MEMORY")).toBe('"copper_lighthouse_memory"');
    expect(isAcceptedHistoryLexicalMatch(
      "COPPER_LIGHTHOUSE_MEMORY",
      "Assistant: COPPER_LIGHTHOUSE_MEMORY",
    )).toBe(true);
  });

  it("validates the public find_threads and read_thread contracts", () => {
    expect(parseHistoryFindThreadsInput({ query: "  copper lighthouse ", limit: 4 })).toEqual({
      query: "copper lighthouse",
      limit: 4,
    });
    expect(parseHistoryReadThreadInput({
      thread_id: "018f1f9a-7b3c-7a09-8000-000000000009",
      turn_ids: ["turn-1", "turn:2"],
    })).toEqual({
      thread_id: "018f1f9a-7b3c-7a09-8000-000000000009",
      turn_ids: ["turn-1", "turn:2"],
    });
    expect(() => parseHistoryFindThreadsInput({
      query: "copper",
      limit: 4,
      agentic: false,
    })).toThrow("supported fields are query and limit");
    expect(() => parseHistoryReadThreadInput({
      thread_id: "not-a-thread",
    })).toThrow("invalid thread id");
  });

  it("seeds Luna with the first retrieval while preserving source coordinates", () => {
    const prompt = seededAgenticSearchPrompt("what changed?", [{
      thread_id: "thread-a",
      title: "Thread A",
      turn_id: "turn-1",
      cursor: "7",
      score: 0.8,
      snippet: "The fallback changed to amber.",
    }]);

    expect(prompt).toContain("Question:\nwhat changed?");
    expect(prompt).toContain("Initial find_threads result (already computed)");
    expect(prompt).toContain('"turn_id":"turn-1"');
    expect(prompt).toContain("Call find_threads only if the initial result is empty or insufficient.");
  });

  it("retains inspected agentic sources in citations beyond the result limit", () => {
    const used = ["turn-1", "turn-2", "turn-3"].map((turn_id, index) => ({
      thread_id: index === 2 ? "thread-b" : "thread-a",
      title: index === 2 ? "Thread B" : "Thread A",
      turn_id,
      cursor: String(index + 1),
      score: 1,
      snippet: `source ${index + 1}`,
    }));

    const evidence = agenticHistoryEvidence(used, 2);

    expect(evidence.results.map((result) => result.turn_id)).toEqual(["turn-1", "turn-2"]);
    expect(evidence.citations).toEqual([
      {
        thread_id: "thread-a",
        title: "Thread A",
        sources: [
          { turn_id: "turn-1", cursor: "1" },
          { turn_id: "turn-2", cursor: "2" },
        ],
      },
      {
        thread_id: "thread-b",
        title: "Thread B",
        sources: [{ turn_id: "turn-3", cursor: "3" }],
      },
    ]);
  });
});
