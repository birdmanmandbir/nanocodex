import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, "/tests")

workspace = Path(os.environ["NANOCODEX_EVAL_WORKSPACE"])
answer = (workspace / "answer.txt").read_text()
case = json.loads(Path("/tests/case.json").read_text())
kind = case["kind"]
evidence = {"kind": kind}

if kind == "gpqa_diamond":
    from official.common import ANSWER_PATTERN_MULTICHOICE

    match = re.search(ANSWER_PATTERN_MULTICHOICE, answer)
    extracted = match.group(1).upper() if match else None
    reward = 1.0 if extracted == case["correct_answer"] else 0.0
    evidence.update(
        {"correct_answer": case["correct_answer"], "extracted_answer": extracted}
    )
else:
    grader_model = os.environ["NANOCODEX_EVAL_GRADER_MODEL"]
    evidence["grader_model"] = grader_model
    if kind == "browsecomp":
        from official.sampler.chat_completion_sampler import (
            OPENAI_SYSTEM_MESSAGE_API,
            ChatCompletionSampler,
        )
        from official.browsecomp_eval import BrowseCompEval

        grader = ChatCompletionSampler(
            model=grader_model,
            system_message=OPENAI_SYSTEM_MESSAGE_API,
            max_tokens=2048,
        )
        evaluator = BrowseCompEval.__new__(BrowseCompEval)
        evaluator.grader_model = grader
        result = evaluator.grade_sample(
            case["question"], case["correct_answer"], answer
        )
        reward = 1.0 if result == "yes" else 0.0
        evidence.update({"grader_result": result})
    elif kind in ("healthbench", "healthbench_professional"):
        from official.healthbench_eval import HealthBenchEval, RubricItem

        if kind == "healthbench_professional":
            from official.sampler.responses_sampler import ResponsesSampler

            grader = ResponsesSampler(
                model=grader_model,
                reasoning_model=True,
                reasoning_effort="low",
            )
            length_center = 2000.0
            length_penalty = 0.0147
        else:
            from official.sampler.chat_completion_sampler import (
                OPENAI_SYSTEM_MESSAGE_API,
                ChatCompletionSampler,
            )

            grader = ChatCompletionSampler(
                model=grader_model,
                system_message=OPENAI_SYSTEM_MESSAGE_API,
                max_tokens=2048,
            )
            length_center = None
            length_penalty = None

        evaluator = HealthBenchEval.__new__(HealthBenchEval)
        evaluator.grader_model = grader
        evaluator.length_adjustment_center = length_center
        evaluator.length_adjustment_penalty_per_500_chars = length_penalty
        metrics, explanation, rubric_grades = evaluator.grade_sample(
            prompt=case["prompt"],
            response_text=answer,
            example_tags=case["example_tags"],
            rubric_items=[RubricItem.from_dict(item) for item in case["rubrics"]],
        )
        reward_name = (
            "overall_score_length_adjusted"
            if kind == "healthbench_professional"
            else "overall_score"
        )
        reward = float(metrics[reward_name])
        evidence.update(
            {
                "metrics": metrics,
                "explanation": explanation,
                "rubric_grades": rubric_grades,
            }
        )
    else:
        raise RuntimeError(f"unknown OpenAI simple-evals case kind: {kind}")

Path("/logs/verifier/official-grader.json").write_text(
    json.dumps(evidence, indent=2)
)
Path("/logs/verifier/reward.json").write_text(json.dumps({"reward": reward}))
